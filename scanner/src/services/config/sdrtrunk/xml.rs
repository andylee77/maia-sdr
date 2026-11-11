//! The XML a playlist is made of: elements, attributes and text, read into a tree, and the
//! escapes for writing one. A name's prefix is dropped (`xsi:type` reads as `type`), namespace
//! declarations are skipped, and a document type is refused (it could define entities).

/// The longest document read (a request's body is no longer).
pub const MAX_BYTES: usize = 2 << 20;
/// The deepest nesting read.
const MAX_DEPTH: usize = 32;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    /// Its own text, trimmed.
    pub text: String,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |c| c.name == name)
    }
}

/// Read a document; the error names the line.
pub fn parse(text: &str) -> Result<Element, String> {
    if text.len() > MAX_BYTES {
        return Err(format!("the file is larger than {} MB", MAX_BYTES >> 20));
    }
    let mut r = Reader { s: text.trim_start_matches('\u{feff}'), at: 0 };
    r.between()?;
    if !r.rest().starts_with('<') {
        return r.err("no element");
    }
    let root = r.element(0)?;
    r.between()?;
    if !r.rest().is_empty() {
        return r.err("text after the document's element");
    }
    Ok(root)
}

struct Reader<'a> {
    s: &'a str,
    at: usize,
}

impl<'a> Reader<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.at..]
    }

    fn err<T>(&self, what: &str) -> Result<T, String> {
        let line = self.s[..self.at].bytes().filter(|&b| b == b'\n').count() + 1;
        Err(format!("line {line}: {what}"))
    }

    fn eat(&mut self, token: &str) -> bool {
        let hit = self.rest().starts_with(token);
        if hit {
            self.at += token.len();
        }
        hit
    }

    fn space(&mut self) {
        let r = self.rest();
        self.at += r.len() - r.trim_start().len();
    }

    /// Past `open` ... `close`, when the text is at `open`.
    fn past(&mut self, open: &str, close: &str) -> Result<bool, String> {
        if !self.rest().starts_with(open) {
            return Ok(false);
        }
        match self.rest().find(close) {
            Some(end) => {
                self.at += end + close.len();
                Ok(true)
            }
            None => self.err(&format!("{open} is not closed")),
        }
    }

    /// Past what stands between elements: white space, comments, processing instructions.
    fn between(&mut self) -> Result<(), String> {
        loop {
            self.space();
            if self.rest().starts_with("<!DOCTYPE") {
                return self.err("a document type is not read");
            }
            if !(self.past("<!--", "-->")? || self.past("<?", "?>")?) {
                return Ok(());
            }
        }
    }

    /// A name as written, with its prefix.
    fn name(&mut self) -> Result<&'a str, String> {
        let r = self.rest();
        let end = r.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))).unwrap_or(r.len());
        if end == 0 {
            return self.err("a name is expected");
        }
        self.at += end;
        Ok(&r[..end])
    }

    /// The element at `<`.
    fn element(&mut self, depth: usize) -> Result<Element, String> {
        if depth > MAX_DEPTH {
            return self.err("elements are nested too deep");
        }
        self.at += 1;
        let mut el = Element { name: local(self.name()?).to_string(), ..Default::default() };
        loop {
            self.space();
            if self.eat("/>") {
                return Ok(el);
            }
            if self.eat(">") {
                break;
            }
            let key = self.name()?;
            self.space();
            if !self.eat("=") {
                return self.err("an attribute needs a value");
            }
            self.space();
            let quote = match self.rest().chars().next() {
                Some(q @ ('"' | '\'')) => q,
                _ => return self.err("an attribute's value is quoted"),
            };
            self.at += 1;
            let Some(end) = self.rest().find(quote) else {
                return self.err("an attribute's value is not closed");
            };
            let value = self.unescape(&self.rest()[..end])?;
            self.at += end + 1;
            if key != "xmlns" && !key.starts_with("xmlns:") {
                el.attrs.push((local(key).to_string(), value));
            }
        }
        loop {
            let r = self.rest();
            let Some(lt) = r.find('<') else {
                return self.err(&format!("<{}> is not closed", el.name));
            };
            el.text.push_str(&self.unescape(&r[..lt])?);
            self.at += lt;
            if self.eat("</") {
                if local(self.name()?) != el.name {
                    return self.err(&format!("<{}> is closed by another element's end", el.name));
                }
                self.space();
                if !self.eat(">") {
                    return self.err("an element's end is not closed");
                }
                break;
            }
            if self.eat("<![CDATA[") {
                let Some(end) = self.rest().find("]]>") else {
                    return self.err("a CDATA section is not closed");
                };
                el.text.push_str(&self.rest()[..end]);
                self.at += end + 3;
            } else if !(self.past("<!--", "-->")? || self.past("<?", "?>")?) {
                el.children.push(self.element(depth + 1)?);
            }
        }
        el.text = el.text.trim().to_string();
        Ok(el)
    }

    /// Text with its `&...;` references replaced.
    fn unescape(&self, raw: &str) -> Result<String, String> {
        if !raw.contains('&') {
            return Ok(raw.to_string());
        }
        let mut out = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(amp) = rest.find('&') {
            out.push_str(&rest[..amp]);
            let Some(end) = rest[amp..].find(';') else {
                return self.err("an & is not a reference (&amp; is the character)");
            };
            let name = &rest[amp + 1..amp + end];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => name
                    .strip_prefix("#x")
                    .map(|h| u32::from_str_radix(h, 16))
                    .or_else(|| name.strip_prefix('#').map(str::parse))
                    .and_then(Result::ok)
                    .and_then(char::from_u32),
            };
            match c {
                Some(c) => out.push(c),
                None => return self.err(&format!("&{name}; is not a reference this reads")),
            }
            rest = &rest[amp + end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }
}

/// A name without its prefix.
fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// Text as an attribute's value or an element's text. Control characters XML has no place for
/// are left out.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c if c < ' ' => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_is_a_tree_of_elements_attributes_and_text() {
        let doc = "\u{feff}<?xml version=\"1.0\" ?>\n<!-- made by hand -->\n<playlist version='4'>\n  <channel name=\"A &amp; B &#x3c;1&#62;\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:type = \"x\">\n    <frequency> 851012500 </frequency><!-- one --><empty/>\n    <note><![CDATA[a < b]]></note>\n  </channel>\n</playlist>\n";
        let root = parse(doc).unwrap();
        assert_eq!((root.name.as_str(), root.attr("version")), ("playlist", Some("4")));
        let ch = root.child("channel").unwrap();
        assert_eq!(ch.attrs, vec![("name".to_string(), "A & B <1>".to_string()), ("type".to_string(), "x".to_string())], "a prefix dropped, the namespace skipped");
        assert_eq!(ch.children("frequency").map(|f| f.text.as_str()).collect::<Vec<_>>(), vec!["851012500"]);
        assert_eq!(ch.child("note").unwrap().text, "a < b");
        assert!(ch.child("empty").unwrap().children.is_empty());
    }

    #[test]
    fn what_is_not_a_document_is_refused_with_its_line() {
        for (doc, why) in [
            ("", "no element"),
            ("just text", "no element"),
            ("<a>\n<b>\n</a>", "line 3: <b> is closed by another element's end"),
            ("<a>", "<a> is not closed"),
            ("<a b=c/>", "an attribute's value is quoted"),
            ("<a b=\"1/>", "an attribute's value is not closed"),
            ("<a>&nbsp;</a>", "&nbsp; is not a reference this reads"),
            ("<a>1 & 2</a>", "an & is not a reference"),
            ("<!DOCTYPE a [<!ENTITY x \"y\">]><a/>", "a document type is not read"),
            ("<a/><b/>", "text after the document's element"),
            ("<a><!-- open</a>", "<!-- is not closed"),
        ] {
            let e = parse(doc).unwrap_err();
            assert!(e.contains(why), "{doc:?}: {e}");
        }
        let deep = format!("{}{}", "<a>".repeat(40), "</a>".repeat(40));
        assert!(parse(&deep).unwrap_err().contains("nested too deep"));
    }

    #[test]
    fn written_text_reads_back() {
        let name = "Fire & Rescue <North> \"A\"\tline\n2\u{1}";
        let doc = format!("<a name=\"{}\"/>", escape(name));
        assert_eq!(parse(&doc).unwrap().attr("name"), Some("Fire & Rescue <North> \"A\"\tline\n2"));
    }
}
