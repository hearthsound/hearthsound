//! The value model a response payload is built from.
//!
//! A payload is an [`Object`]: an ordered list of named members. Members keep
//! the order they were added in, and both encodings render them in that order,
//! so a response is byte-stable and its snapshot is readable. Nothing here
//! knows about JSON or XML; [`crate::render`] turns these values into both.

use std::borrow::Cow;

/// The member name the JSON encoding gives an element's content.
pub(crate) const CONTENT_MEMBER: &str = "value";

/// A value in a response payload.
///
/// The variants are the four scalar kinds the protocol needs plus the two
/// containers. Values convert into a member with `into()`, so a payload reads
/// as a list of named members.
///
/// # Examples
///
/// ```
/// use hearthsound_api_subsonic::{Object, Value};
///
/// let folder = Object::new()
///     .set("id", 1)
///     .set("name", "My Library");
/// assert_eq!(
///     folder.members()[0],
///     ("id".into(), Value::Int(1)),
/// );
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A boolean.
    Bool(bool),
    /// A signed integer.
    ///
    /// This is the only integer kind: keeping one makes an integer literal's
    /// type inferrable at a call site. A caller with an unsigned value converts
    /// it explicitly, for example `i64::from(depth)`.
    Int(i64),
    /// A floating point number.
    Float(f64),
    /// A string.
    Str(String),
    /// A string the XML encoding writes as a child element of its parent.
    ///
    /// A plain [`Value::Str`] member is an XML attribute, which is right for a
    /// field the schema describes as an attribute, such as a song's title. A
    /// field the schema describes as a child element, such as `getArtistInfo`'s
    /// `biography`, is set with [`Object::set_element`] instead. Both encodings
    /// write it as a string.
    Element(String),
    /// The text content of the element being built.
    ///
    /// The XML encoding writes it inside the element, after its attributes, and
    /// the element is never self-closing. The JSON encoding writes it as the
    /// member named `value`. [`Object::set_content`] sets it.
    Content(String),
    /// A list of values.
    Array(Array),
    /// A set of named members.
    Object(Object),
}

impl Value {
    /// Reports whether the value is a scalar, which an XML element carries as
    /// an attribute rather than as a child element.
    ///
    /// # Returns
    ///
    /// `true` for the boolean, integer, float, and string variants.
    pub(crate) fn is_scalar(&self) -> bool {
        matches!(
            self,
            Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Str(_)
        )
    }
}

impl From<bool> for Value {
    /// Wraps a boolean.
    fn from(value: bool) -> Value {
        Value::Bool(value)
    }
}

impl From<i64> for Value {
    /// Wraps a signed integer.
    fn from(value: i64) -> Value {
        Value::Int(value)
    }
}

impl From<f64> for Value {
    /// Wraps a floating point number.
    fn from(value: f64) -> Value {
        Value::Float(value)
    }
}

impl From<&str> for Value {
    /// Wraps a borrowed string, which is copied.
    fn from(value: &str) -> Value {
        Value::Str(value.to_owned())
    }
}

impl From<String> for Value {
    /// Wraps an owned string.
    fn from(value: String) -> Value {
        Value::Str(value)
    }
}

impl From<Object> for Value {
    /// Wraps an object.
    fn from(value: Object) -> Value {
        Value::Object(value)
    }
}

impl From<Array> for Value {
    /// Wraps an array.
    fn from(value: Array) -> Value {
        Value::Array(value)
    }
}

impl From<Vec<Value>> for Value {
    /// Wraps values as unnamed array items.
    ///
    /// # Parameters
    ///
    /// - `values`: the items, in order.
    ///
    /// # Returns
    ///
    /// An [`Array`] that takes its XML element name from the member it is set
    /// as. See [`Array::named`] for arrays whose elements are named differently.
    fn from(values: Vec<Value>) -> Value {
        Value::Array(Array::new(values))
    }
}

/// An ordered set of named members.
///
/// Setting a member that is already present replaces its value in place, so an
/// object never carries the same name twice and a member's position is the
/// position it was first added at.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Object {
    /// The members, in the order they were added.
    members: Vec<(Cow<'static, str>, Value)>,
}

impl Object {
    /// Builds an empty object.
    ///
    /// # Returns
    ///
    /// The object, with no members.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_api_subsonic::Object;
    ///
    /// let license = Object::new().set("valid", true);
    /// assert_eq!(license.len(), 1);
    /// ```
    pub fn new() -> Object {
        Object {
            members: Vec::new(),
        }
    }

    /// Adds a member and returns the object.
    ///
    /// # Parameters
    ///
    /// - `name`: the member name, which is both the JSON key and, unless an
    ///   array overrides it, the XML element or attribute name.
    /// - `value`: the member value; anything that converts into a [`Value`].
    ///
    /// # Returns
    ///
    /// The object, with the member added last or replaced where it already was.
    pub fn set<N, V>(mut self, name: N, value: V) -> Object
    where
        N: Into<Cow<'static, str>>,
        V: Into<Value>,
    {
        let name = name.into();
        let value = value.into();
        match self.members.iter_mut().find(|(member, _)| *member == name) {
            Some((_, existing)) => *existing = value,
            None => self.members.push((name, value)),
        }
        self
    }

    /// Returns the members, in order.
    pub fn members(&self) -> &[(Cow<'static, str>, Value)] {
        &self.members
    }

    /// Adds a member the XML encoding writes as a child element.
    ///
    /// The JSON encoding writes it as a string member, exactly like
    /// [`Object::set`] with a string.
    ///
    /// # Parameters
    ///
    /// - `name`: the member name, which is both the JSON key and the XML
    ///   element name.
    /// - `text`: the element's text.
    ///
    /// # Returns
    ///
    /// The object, with the member added last or replaced where it already was.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_api_subsonic::Object;
    ///
    /// let artist = Object::new().set_element("biography", "From Iceland");
    /// assert_eq!(artist.members()[0].0, "biography");
    /// ```
    pub fn set_element<N, S>(self, name: N, text: S) -> Object
    where
        N: Into<Cow<'static, str>>,
        S: Into<String>,
    {
        self.set(name, Value::Element(text.into()))
    }

    /// Sets the element's own text content.
    ///
    /// Only one content makes sense, so setting it again replaces the text. The
    /// XML encoding writes it inside the element, after its attributes, and the
    /// element is never self-closing; the JSON encoding writes it as the member
    /// named `value`.
    ///
    /// # Parameters
    ///
    /// - `text`: the element's content.
    ///
    /// # Returns
    ///
    /// The object, with its content set.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_api_subsonic::Object;
    ///
    /// let genres = Object::new().set_content("rock, pop");
    /// assert_eq!(genres.members()[0].0, "value");
    /// ```
    pub fn set_content(self, text: impl Into<String>) -> Object {
        self.set(CONTENT_MEMBER, Value::Content(text.into()))
    }

    /// Returns the number of members.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Reports whether the object has no members.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

impl<N, V> FromIterator<(N, V)> for Object
where
    N: Into<Cow<'static, str>>,
    V: Into<Value>,
{
    /// Builds an object from named members, in iteration order.
    fn from_iter<I: IntoIterator<Item = (N, V)>>(members: I) -> Object {
        members
            .into_iter()
            .fold(Object::new(), |object, (name, value)| {
                object.set(name, value)
            })
    }
}

/// A list of values.
///
/// The item element name is what an XML encoding names each element after; it
/// defaults to the name of the member the array is set as, which is what the
/// schema almost always wants.
#[derive(Debug, Clone, PartialEq)]
pub struct Array {
    /// The XML element name of each item, when it differs from the member name.
    item: Option<Cow<'static, str>>,
    /// The items, in order.
    values: Vec<Value>,
}

impl Array {
    /// Builds an array whose XML elements are named after the member it is set
    /// as.
    ///
    /// # Parameters
    ///
    /// - `values`: the items, in order.
    ///
    /// # Returns
    ///
    /// The array.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_api_subsonic::{Array, Object};
    ///
    /// let folders = Array::new([
    ///     Object::new().set("id", 1),
    ///     Object::new().set("id", 2),
    /// ]);
    /// assert_eq!(folders.len(), 2);
    /// ```
    pub fn new(values: impl IntoIterator<Item = impl Into<Value>>) -> Array {
        Array {
            item: None,
            values: values.into_iter().map(Into::into).collect(),
        }
    }

    /// Builds an array whose XML elements carry an explicit name.
    ///
    /// Use this where the JSON member name is plural and the schema's element
    /// name is singular, as in `openSubsonicExtensions` holding
    /// `openSubsonicExtension` elements.
    ///
    /// # Parameters
    ///
    /// - `item`: the XML element name of each item.
    /// - `values`: the items, in order.
    ///
    /// # Returns
    ///
    /// The array.
    pub fn named<N>(item: N, values: impl IntoIterator<Item = impl Into<Value>>) -> Array
    where
        N: Into<Cow<'static, str>>,
    {
        Array {
            item: Some(item.into()),
            values: values.into_iter().map(Into::into).collect(),
        }
    }

    /// Appends an item.
    ///
    /// # Parameters
    ///
    /// - `value`: the item; anything that converts into a [`Value`].
    pub fn push(&mut self, value: impl Into<Value>) {
        self.values.push(value.into());
    }

    /// Returns the XML element name of each item, when it is not the member
    /// name.
    ///
    /// # Returns
    ///
    /// The name given to [`Array::named`], or `None`.
    pub fn item_name(&self) -> Option<&str> {
        self.item.as_deref()
    }

    /// Returns the items, in order.
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// Returns the number of items.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Reports whether the array has no items.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_a_member_twice_keeps_its_position() {
        let object = Object::new()
            .set("id", 1)
            .set("name", "first")
            .set("id", 7)
            .set("name", "second");
        assert_eq!(
            object.members(),
            [
                ("id".into(), Value::Int(7)),
                ("name".into(), Value::Str("second".to_owned())),
            ]
        );
    }

    #[test]
    fn an_object_collects_members_in_iteration_order() {
        let object: Object = [("b", 2), ("a", 1)].into_iter().collect();
        let names: Vec<&str> = object
            .members()
            .iter()
            .map(|(name, _)| name.as_ref())
            .collect();
        assert_eq!(names, ["b", "a"]);
    }

    #[test]
    fn scalar_members_are_the_ones_an_xml_element_carries_as_attributes() {
        assert!(Value::from(true).is_scalar());
        assert!(Value::from(1).is_scalar());
        assert!(Value::from(1.5).is_scalar());
        assert!(Value::from("text").is_scalar());
        assert!(!Value::from(Array::new([1])).is_scalar());
        assert!(!Value::from(Object::new()).is_scalar());
    }
}
