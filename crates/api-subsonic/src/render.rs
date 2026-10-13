//! The two encodings of a payload value.
//!
//! Both encodings are written from the same [`Value`] tree, so a payload has
//! exactly the same fields in JSON and in XML. The JSON writer emits an object
//! as `{...}` and an array as `[...]`; the XML writer follows the base
//! protocol's shape, where an object is one element, its scalar members are
//! attributes, and its nested members are child elements. See the crate
//! documentation for the rules in full.

use std::fmt::Write as _;

use crate::value::Value;

/// Writes a value as JSON.
///
/// # Parameters
///
/// - `out`: the buffer the document is appended to.
/// - `value`: the value; objects and arrays are written recursively.
pub(crate) fn write_json(out: &mut String, value: &Value) {
    match value {
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(number) => write_number(out, *number),
        Value::Float(number) => write_float(out, *number),
        Value::Str(text) => write_json_string(out, text),
        Value::Array(array) => {
            out.push('[');
            for (index, item) in array.values().iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(out, item);
            }
            out.push(']');
        }
        Value::Object(object) => {
            out.push('{');
            for (index, (name, value)) in object.members().iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json_string(out, name);
                out.push(':');
                write_json(out, value);
            }
            out.push('}');
        }
    }
}

/// Writes a string as a quoted JSON string.
///
/// # Parameters
///
/// - `out`: the buffer the string is appended to.
/// - `text`: the string; quotes, backslashes, and control characters are
///   escaped, everything else is written as the UTF-8 it already is.
pub(crate) fn write_json_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

/// Writes one member of an object as XML.
///
/// An array member becomes one element per item, named after the member itself
/// unless the array carries an explicit item name.
///
/// # Parameters
///
/// - `out`: the buffer the elements are appended to.
/// - `name`: the member name.
/// - `value`: the member value.
pub(crate) fn write_member(out: &mut String, name: &str, value: &Value) {
    match value {
        Value::Array(array) => {
            let item = array.item_name().unwrap_or(name);
            for value in array.values() {
                write_element(out, item, value);
            }
        }
        value => write_element(out, name, value),
    }
}

/// Writes a value as one XML element.
///
/// An object's scalar members become attributes and its nested members become
/// child elements; an element with neither is self-closing. An array written
/// here repeats its items, named after `name`.
///
/// # Parameters
///
/// - `out`: the buffer the element is appended to.
/// - `name`: the element name, which must be a valid XML name.
/// - `value`: the element's value.
fn write_element(out: &mut String, name: &str, value: &Value) {
    match value {
        Value::Object(object) => {
            out.push('<');
            out.push_str(name);
            for (member, value) in object.members() {
                if value.is_scalar() {
                    out.push(' ');
                    out.push_str(member);
                    out.push_str("=\"");
                    write_attribute_value(out, value);
                    out.push('"');
                }
            }
            if object.members().iter().all(|(_, value)| value.is_scalar()) {
                out.push_str("/>");
                return;
            }
            out.push('>');
            for (member, value) in object.members() {
                if !value.is_scalar() {
                    write_member(out, member, value);
                }
            }
            close_element(out, name);
        }
        Value::Array(array) => {
            let item = array.item_name().unwrap_or(name);
            for value in array.values() {
                write_element(out, item, value);
            }
        }
        scalar => {
            out.push('<');
            out.push_str(name);
            out.push('>');
            write_text(out, scalar);
            close_element(out, name);
        }
    }
}

/// Writes one scalar as an XML attribute value.
///
/// # Parameters
///
/// - `out`: the buffer the value is appended to.
/// - `value`: the attribute value. Only the envelope's own fields are written
///   this way, so a non-scalar writes nothing.
pub(crate) fn write_attribute_value(out: &mut String, value: &Value) {
    match value {
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(number) => write_number(out, *number),
        Value::Float(number) => {
            if number.is_finite() {
                write_number(out, *number)
            }
        }
        Value::Str(text) => escape_attribute(out, text),
        Value::Array(_) | Value::Object(_) => {}
    }
}

/// Writes a scalar as the text of an XML element.
fn write_text(out: &mut String, value: &Value) {
    match value {
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(number) => write_number(out, *number),
        Value::Float(number) => {
            if number.is_finite() {
                write_number(out, *number)
            }
        }
        Value::Str(text) => escape_text(out, text),
        Value::Array(_) | Value::Object(_) => {}
    }
}

/// Writes the closing tag of an element.
fn close_element(out: &mut String, name: &str) {
    out.push_str("</");
    out.push_str(name);
    out.push('>');
}

/// Writes a number in the shortest form that round-trips.
///
/// A floating point number is written without a trailing `.0`, so `4.0` is
/// written as `4`; a client parses a JSON number either way.
fn write_number(out: &mut String, number: impl std::fmt::Display) {
    let _ = write!(out, "{number}");
}

/// Writes a floating point number as JSON.
///
/// A non-finite number has no JSON representation and is written as `null`;
/// response payloads never carry one.
fn write_float(out: &mut String, number: f64) {
    if number.is_finite() {
        write_number(out, number);
    } else {
        out.push_str("null");
    }
}

/// Escapes text for an XML text node.
fn escape_text(out: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            character if is_forbidden_control(character) => {}
            character => out.push(character),
        }
    }
}

/// Escapes text for an XML attribute value.
///
/// Whitespace is escaped as a character reference as well, because an XML
/// parser normalizes literal whitespace in an attribute value.
fn escape_attribute(out: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            character if is_forbidden_control(character) => {}
            character => out.push(character),
        }
    }
}

/// Reports whether a character cannot appear in an XML 1.0 document at all.
fn is_forbidden_control(character: char) -> bool {
    (character as u32) < 0x20 && !matches!(character, '\t' | '\n' | '\r')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Array, Object};

    /// Renders an object's members as XML children.
    fn xml_members(object: &Object) -> String {
        let mut out = String::new();
        for (name, value) in object.members() {
            write_member(&mut out, name, value);
        }
        out
    }

    #[test]
    fn json_escapes_the_characters_it_must() {
        let mut out = String::new();
        write_json(&mut out, &Value::from("a\"b\\c\nd\te\u{1}f"));
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\\te\\u0001f\"");
    }

    #[test]
    fn xml_escapes_text_and_attributes_differently() {
        let object = Object::new()
            .set("name", "Roses & Bones <live>")
            .set("comment", "quoted \"text\"\nsecond line");
        assert_eq!(
            xml_members(&object),
            "<name>Roses &amp; Bones &lt;live&gt;</name><comment>quoted \"text\"\nsecond line</comment>"
        );
    }

    #[test]
    fn an_object_becomes_one_element_with_attributes_and_children() {
        let album = Object::new()
            .set("id", 7)
            .set("name", "Album")
            .set("artist", Object::new().set("id", 1));
        let mut out = String::new();
        write_element(&mut out, "album", &Value::from(album));
        assert_eq!(
            out,
            "<album id=\"7\" name=\"Album\"><artist id=\"1\"/></album>"
        );
    }

    #[test]
    fn a_scalar_member_becomes_a_text_element() {
        let object = Object::new().set("name", "Index A");
        assert_eq!(xml_members(&object), "<name>Index A</name>");
    }

    #[test]
    fn an_array_becomes_one_element_per_item_named_after_the_member() {
        let folders = Array::new([
            Object::new().set("id", 1).set("name", "My Library"),
            Object::new().set("id", 2).set("name", "Server"),
        ]);
        let wrapper = Object::new().set("musicFolders", Object::new().set("musicFolder", folders));
        assert_eq!(
            xml_members(&wrapper),
            "<musicFolders><musicFolder id=\"1\" name=\"My Library\"/><musicFolder id=\"2\" name=\"Server\"/></musicFolders>"
        );
    }

    #[test]
    fn an_array_can_rename_its_items() {
        let extensions = Array::named(
            "openSubsonicExtension",
            [Object::new().set("name", "formPost")],
        );
        let mut out = String::new();
        write_member(&mut out, "openSubsonicExtensions", &Value::from(extensions));
        assert_eq!(out, "<openSubsonicExtension name=\"formPost\"/>");
    }

    #[test]
    fn a_scalar_array_becomes_repeated_elements() {
        let versions = Array::new([1, 2]);
        let mut out = String::new();
        write_member(&mut out, "versions", &Value::from(versions));
        assert_eq!(out, "<versions>1</versions><versions>2</versions>");
    }

    #[test]
    fn an_empty_array_and_an_empty_object_have_no_xml_content() {
        let mut out = String::new();
        write_member(
            &mut out,
            "isrc",
            &Value::from(Array::new(Vec::<Value>::new())),
        );
        assert_eq!(out, "");
        write_element(&mut out, "replayGain", &Value::from(Object::new()));
        assert_eq!(out, "<replayGain/>");
    }

    #[test]
    fn floats_are_written_in_their_shortest_round_tripping_form() {
        let mut json = String::new();
        write_json(&mut json, &Value::from(0.1));
        write_json(&mut json, &Value::from(4.0));
        write_json(&mut json, &Value::from(f64::NAN));
        assert_eq!(json, "0.14null");

        let mut xml = String::new();
        write_element(&mut xml, "trackGain", &Value::from(0.1));
        assert_eq!(xml, "<trackGain>0.1</trackGain>");
    }

    #[test]
    fn control_characters_that_xml_forbids_are_dropped() {
        let mut out = String::new();
        write_element(&mut out, "comment", &Value::from("keep\u{0}drop\u{1f}"));
        assert_eq!(out, "<comment>keepdrop</comment>");
    }

    #[test]
    fn attribute_values_escape_their_whitespace() {
        let mut out = String::new();
        write_attribute_value(&mut out, &Value::from("a\nb\tc&d"));
        assert_eq!(out, "a&#10;b&#9;c&amp;d");
    }
}
