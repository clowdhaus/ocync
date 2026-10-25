//! Reading text out of the small XML documents the AWS query protocol answers
//! with, such as STS's `AssumeRoleResponse` and `ErrorResponse`.
//!
//! These documents have no attributes, comments, CDATA or repeated nesting of
//! one element name, so finding an element by name is enough.

/// The text of the element reached by `path`, each name nested inside the
/// one before it, with entities replaced. `None` when an element is missing
/// or the text holds an unknown entity.
pub(super) fn text(document: &str, path: &[&str]) -> Option<String> {
    let inner = path
        .iter()
        .try_fold(document, |scope, name| element(scope, name))?;
    unescape(inner)
}

/// The content of the first `<name>` element in `scope`.
fn element<'a>(scope: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}");
    let mut from = 0;
    loop {
        let start = from + scope[from..].find(&open)?;
        let after = &scope[start + open.len()..];
        match after.chars().next()? {
            '>' => {
                let content = &after[1..];
                let end = content.find(&format!("</{name}>"))?;
                return Some(&content[..end]);
            }
            '/' if after.starts_with("/>") => return Some(""),
            // A longer name sharing the prefix, such as `<CodeX>`.
            _ => from = start + open.len(),
        }
    }
}

/// `text` with the five named entities and numeric references replaced.
fn unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let end = rest[at..].find(';')? + at;
        let entity = &rest[at + 1..end];
        out.push(match entity {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = match entity.strip_prefix("#x") {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => entity.strip_prefix('#')?.parse().ok()?,
                };
                char::from_u32(code)?
            }
        });
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text is found by nesting, a longer name sharing a prefix is skipped,
    /// a self-closed element is empty, and entities are replaced. Breaks if
    /// `<CodeX>` is read as `<Code>` or `&amp;` survives.
    #[test]
    fn reads_nested_text() {
        let doc = "<ErrorResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\">\
            <Error><Type>Sender</Type><CodeX>no</CodeX><Code>InvalidIdentityToken</Code>\
            <Message>a &amp; b &lt;c&gt; &#65;&#x42;</Message><Detail/></Error></ErrorResponse>";
        assert_eq!(
            text(doc, &["Error", "Code"]).as_deref(),
            Some("InvalidIdentityToken")
        );
        assert_eq!(
            text(doc, &["Error", "Message"]).as_deref(),
            Some("a & b <c> AB")
        );
        assert_eq!(text(doc, &["Error", "Detail"]).as_deref(), Some(""));
        assert_eq!(text(doc, &["Error", "RequestId"]), None);
        assert_eq!(text("<A>&bogus;</A>", &["A"]), None);
    }
}
