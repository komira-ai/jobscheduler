//! The XML answers of the EC2 Query API, read once into a small tree. Names are local
//! names (the namespace of the root is dropped) and a list is a node whose `item`
//! children are its elements.

use std::str::FromStr;

use quick_xml::Reader;
use quick_xml::escape::unescape;
use quick_xml::events::Event;

use crate::error::ApiError;

/// The error of an answer that is not what the documentation says, in the crate's words.
pub(crate) fn unreadable(what: impl Into<String>) -> ApiError {
    ApiError::Unreadable(what.into())
}

/// One element: its name, the text directly inside it and its child elements.
pub(crate) struct Node {
    name: String,
    text: String,
    children: Vec<Node>,
}

impl Node {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            text: String::new(),
            children: Vec::new(),
        }
    }

    /// The first child named `name`.
    pub(crate) fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|child| child.name == name)
    }

    /// The children named `name`, in document order.
    pub(crate) fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |child| child.name == name)
    }

    /// The element at `path`, names joined by `/`, each the first child of the last.
    pub(crate) fn at(&self, path: &str) -> Option<&Node> {
        path.split('/')
            .try_fold(self, |node, name| node.child(name))
    }

    /// The first element named `name` at any depth below this one.
    pub(crate) fn find(&self, name: &str) -> Option<&Node> {
        self.children.iter().find_map(|child| {
            (child.name == name)
                .then_some(child)
                .or_else(|| child.find(name))
        })
    }

    /// The `item` elements of the list at `list` (a path, as for [`Node::at`]); none when
    /// the list is missing.
    pub(crate) fn items(&self, list: &str) -> impl Iterator<Item = &Node> {
        self.at(list)
            .into_iter()
            .flat_map(|list| list.children("item"))
    }

    /// The trimmed text directly inside this element.
    pub(crate) fn text(&self) -> &str {
        self.text.trim()
    }

    /// The trimmed text of the element at `path`, when there is one.
    pub(crate) fn text_at(&self, path: &str) -> Option<&str> {
        self.at(path).map(Node::text)
    }

    /// The text of the element at `path`.
    ///
    /// # Errors
    ///
    /// [`ApiError::Unreadable`] when the element is missing or empty.
    pub(crate) fn required(&self, path: &str) -> Result<&str, ApiError> {
        self.text_at(path)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| unreadable(format!("{path} is missing")))
    }

    /// The number the element at `path` holds.
    ///
    /// # Errors
    ///
    /// [`ApiError::Unreadable`] when the element is missing or not a number.
    pub(crate) fn number<T: FromStr>(&self, path: &str) -> Result<T, ApiError> {
        self.required(path)?
            .parse()
            .map_err(|_| unreadable(format!("{path} is not a number")))
    }
}

/// Adds `text` to the element being read, or to the document before its root.
fn append(stack: &mut [Node], text: &str) {
    stack
        .last_mut()
        .into_iter()
        .for_each(|open| open.text.push_str(text));
}

/// Closes the element being read and makes it a child of the one around it.
fn close(stack: &mut Vec<Node>) {
    let closed = stack.pop();
    stack
        .last_mut()
        .into_iter()
        .zip(closed)
        .for_each(|(parent, node)| parent.children.push(node));
}

/// The root element of the document `bytes` (the first, were there more).
///
/// # Errors
///
/// [`ApiError::Unreadable`] when the bytes are not UTF-8, not well-formed XML, hold a
/// reference to an entity that is not defined, or hold no closed root element.
pub(crate) fn parse(bytes: &[u8]) -> Result<Node, ApiError> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| unreadable("the answer is not UTF-8 text"))?;
    let mut reader = Reader::from_str(text);
    reader.config_mut().expand_empty_elements = true;
    let mut stack = vec![Node::new("")];
    loop {
        let event = reader
            .read_event()
            .map_err(|_| unreadable("the answer is not well-formed XML"))?;
        match event {
            Event::Start(start) => stack.push(Node::new(start.local_name().into_inner())),
            Event::End(_) => close(&mut stack),
            Event::Text(inner) => append(&mut stack, &inner.xml10_content()),
            Event::CData(inner) => append(&mut stack, &inner.xml10_content()),
            Event::GeneralRef(reference) => {
                let entity = format!("&{};", &*reference);
                let resolved = unescape(&entity)
                    .map_err(|_| unreadable("the answer holds an entity that is not defined"))?;
                append(&mut stack, &resolved);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let closed = stack.len() == 1;
    stack
        .into_iter()
        .next()
        .and_then(|document| document.children.into_iter().next())
        .filter(|_| closed)
        .ok_or_else(|| unreadable("the answer holds no complete document"))
}
