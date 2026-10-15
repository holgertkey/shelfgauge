//! A small arena DOM that `html5ever` builds (its [`TreeSink`]).
//!
//! The text is fed to html5ever in chunks of [`CHUNK`] bytes; when the tree gets deeper
//! than [`MAX_DEPTH`] parsing stops with an error (the HTML tree builder checks the
//! stack of open elements for most tags, so very deep nesting would take quadratic
//! time), and so it does when it has more than [`MAX_NODES`] nodes (bounding memory).
//!
//! Nodes live in one `Vec` and refer to each other by index (parent, first and last
//! child, previous and next sibling), so every tree operation is O(1) and dropping the
//! tree never recurses. Comments, processing instructions and the doctype are not
//! kept. The sink never panics: html5ever only asks for element names of elements and
//! template contents of templates, and anything else gets a harmless answer.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};

use html5ever::interface::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, LocalName, QualName, ns, parse_document};

/// A node index.
pub(super) type Id = usize;

/// The document node.
pub(super) const DOCUMENT: Id = 0;

/// The deepest element nesting accepted.
pub(super) const MAX_DEPTH: u32 = 512;

/// The most nodes a page may have (far beyond any book; the XHTML stage would refuse
/// much more anyway).
pub(super) const MAX_NODES: usize = 1_000_000;

/// Bytes of text handed to the parser at a time.
const CHUNK: usize = 64 * 1024;

/// Why a page was not parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TooBig {
    /// Elements are nested more than [`MAX_DEPTH`] levels deep.
    Deep,
    /// The page has more than [`MAX_NODES`] nodes.
    Large,
}

/// What a node is.
#[derive(Debug, Clone)]
pub(super) enum Data {
    /// The document (also the content of a `<template>`, never attached).
    Document,
    /// An element.
    Element {
        /// The tag name.
        name: QualName,
        /// The attributes.
        attrs: Vec<Attribute>,
    },
    /// Text.
    Text(StrTendril),
    /// A comment, processing instruction or doctype (not used).
    Other,
}

/// One node of the tree.
#[derive(Debug, Clone)]
pub(super) struct Node {
    pub data: Data,
    pub parent: Option<Id>,
    pub first_child: Option<Id>,
    pub last_child: Option<Id>,
    pub previous: Option<Id>,
    pub next: Option<Id>,
    /// Depth below the document when the node was last inserted.
    pub depth: u32,
}

/// The parsed document.
#[derive(Debug, Default)]
pub(super) struct Dom {
    pub nodes: Vec<Node>,
}

impl Dom {
    /// Parses `text` as an HTML document.
    pub fn parse(text: &str) -> Result<Self, TooBig> {
        let sink = Sink {
            dom: RefCell::new(Dom {
                nodes: vec![Node::new(Data::Document)],
            }),
            deepest: Cell::new(0),
        };
        let mut parser = parse_document(sink, Default::default());
        let mut rest = text;
        while !rest.is_empty() {
            let mut end = rest.len().min(CHUNK);
            while !rest.is_char_boundary(end) {
                end += 1;
            }
            parser.process(StrTendril::from_slice(&rest[..end]));
            rest = &rest[end..];
            let sink = &parser.tokenizer.sink.sink;
            if sink.deepest.get() > MAX_DEPTH {
                return Err(TooBig::Deep);
            }
            if sink.dom.borrow().nodes.len() > MAX_NODES {
                return Err(TooBig::Large);
            }
        }
        let dom = parser.finish();
        Ok(dom)
    }

    /// The children of `id`.
    pub fn children(&self, id: Id) -> Children<'_> {
        Children {
            dom: self,
            next: self.nodes[id].first_child,
        }
    }

    /// The local name of an element (lowercase for HTML elements); `None` otherwise.
    pub fn name(&self, id: Id) -> Option<&str> {
        match &self.nodes[id].data {
            Data::Element { name, .. } => Some(&name.local),
            _ => None,
        }
    }

    /// The value of the attribute `name` (no namespace) of an element.
    pub fn attr(&self, id: Id, name: &str) -> Option<&str> {
        match &self.nodes[id].data {
            Data::Element { attrs, .. } => attrs
                .iter()
                .find(|a| a.name.ns == ns!() && &*a.name.local == name)
                .map(|a| &*a.value),
            _ => None,
        }
    }

    /// The first element named `name` in document order (iterative).
    pub fn find(&self, name: &str) -> Option<Id> {
        self.descendants(DOCUMENT)
            .find(|&id| self.name(id) == Some(name))
    }

    /// All nodes below `id` in document order, `id` excluded.
    pub fn descendants(&self, id: Id) -> impl Iterator<Item = Id> + '_ {
        let mut stack: Vec<Id> = self.children(id).collect();
        stack.reverse();
        std::iter::from_fn(move || {
            let next = stack.pop()?;
            let start = stack.len();
            stack.extend(self.children(next));
            stack[start..].reverse();
            Some(next)
        })
    }

    fn push(&mut self, data: Data) -> Id {
        self.nodes.push(Node::new(data));
        self.nodes.len() - 1
    }

    fn detach(&mut self, id: Id) {
        let Node {
            parent,
            previous,
            next,
            ..
        } = self.nodes[id];
        match previous {
            Some(p) => self.nodes[p].next = next,
            None => {
                if let Some(parent) = parent {
                    self.nodes[parent].first_child = next;
                }
            }
        }
        match next {
            Some(n) => self.nodes[n].previous = previous,
            None => {
                if let Some(parent) = parent {
                    self.nodes[parent].last_child = previous;
                }
            }
        }
        let node = &mut self.nodes[id];
        node.parent = None;
        node.previous = None;
        node.next = None;
    }

    fn append(&mut self, parent: Id, child: Id) {
        self.detach(child);
        self.nodes[child].depth = self.nodes[parent].depth + 1;
        let last = self.nodes[parent].last_child;
        self.nodes[child].parent = Some(parent);
        self.nodes[child].previous = last;
        match last {
            Some(last) => self.nodes[last].next = Some(child),
            None => self.nodes[parent].first_child = Some(child),
        }
        self.nodes[parent].last_child = Some(child);
    }

    fn insert_before(&mut self, sibling: Id, child: Id) {
        self.detach(child);
        let parent = self.nodes[sibling].parent;
        let previous = self.nodes[sibling].previous;
        self.nodes[child].depth = self.nodes[sibling].depth;
        self.nodes[child].parent = parent;
        self.nodes[child].previous = previous;
        self.nodes[child].next = Some(sibling);
        self.nodes[sibling].previous = Some(child);
        match previous {
            Some(p) => self.nodes[p].next = Some(child),
            None => {
                if let Some(parent) = parent {
                    self.nodes[parent].first_child = Some(child);
                }
            }
        }
    }

    /// Appends text to the text node `id` if it is one.
    fn extend_text(&mut self, id: Option<Id>, text: &StrTendril) -> bool {
        if let Some(id) = id
            && let Data::Text(existing) = &mut self.nodes[id].data
        {
            existing.push_tendril(text);
            return true;
        }
        false
    }
}

impl Node {
    fn new(data: Data) -> Self {
        Self {
            data,
            parent: None,
            first_child: None,
            last_child: None,
            previous: None,
            next: None,
            depth: 0,
        }
    }
}

/// The children of a node.
pub(super) struct Children<'a> {
    dom: &'a Dom,
    next: Option<Id>,
}

impl Iterator for Children<'_> {
    type Item = Id;

    fn next(&mut self) -> Option<Id> {
        let id = self.next?;
        self.next = self.dom.nodes[id].next;
        Some(id)
    }
}

struct Sink {
    dom: RefCell<Dom>,
    /// The greatest depth of an inserted node.
    deepest: Cell<u32>,
}

impl Sink {
    fn note_depth(&self, dom: &Dom, id: Id) {
        self.deepest
            .set(self.deepest.get().max(dom.nodes[id].depth));
    }
}

/// An element name handed to html5ever.
#[derive(Debug)]
struct Name(QualName);

impl html5ever::interface::tree_builder::ElemName for Name {
    fn ns(&self) -> &html5ever::Namespace {
        &self.0.ns
    }

    fn local_name(&self) -> &LocalName {
        &self.0.local
    }
}

impl TreeSink for Sink {
    type Handle = Id;
    type Output = Dom;
    type ElemName<'a> = Name;

    fn finish(self) -> Dom {
        self.dom.into_inner()
    }

    fn parse_error(&self, _: Cow<'static, str>) {}

    fn get_document(&self) -> Id {
        DOCUMENT
    }

    fn elem_name<'a>(&'a self, target: &'a Id) -> Name {
        match &self.dom.borrow().nodes[*target].data {
            Data::Element { name, .. } => Name(name.clone()),
            _ => Name(QualName::new(None, ns!(), LocalName::from(""))),
        }
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> Id {
        let mut dom = self.dom.borrow_mut();
        let id = dom.push(Data::Element { name, attrs });
        if flags.template {
            // The template's contents: a document of its own, right after it.
            dom.push(Data::Document);
        }
        id
    }

    fn create_comment(&self, _: StrTendril) -> Id {
        self.dom.borrow_mut().push(Data::Other)
    }

    fn create_pi(&self, _: StrTendril, _: StrTendril) -> Id {
        self.dom.borrow_mut().push(Data::Other)
    }

    fn append(&self, parent: &Id, child: NodeOrText<Id>) {
        let mut dom = self.dom.borrow_mut();
        match child {
            NodeOrText::AppendNode(node) => {
                dom.append(*parent, node);
                self.note_depth(&dom, node);
            }
            NodeOrText::AppendText(text) => {
                let last = dom.nodes[*parent].last_child;
                if !dom.extend_text(last, &text) {
                    let node = dom.push(Data::Text(text));
                    dom.append(*parent, node);
                }
            }
        }
    }

    fn append_based_on_parent_node(&self, element: &Id, previous: &Id, child: NodeOrText<Id>) {
        let has_parent = self.dom.borrow().nodes[*element].parent.is_some();
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(previous, child);
        }
    }

    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {}

    fn get_template_contents(&self, target: &Id) -> Id {
        let dom = self.dom.borrow();
        match dom.nodes.get(target + 1) {
            Some(Node {
                data: Data::Document,
                ..
            }) if matches!(dom.nodes[*target].data, Data::Element { .. }) => target + 1,
            _ => DOCUMENT,
        }
    }

    fn same_node(&self, x: &Id, y: &Id) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &Id, child: NodeOrText<Id>) {
        let mut dom = self.dom.borrow_mut();
        match child {
            NodeOrText::AppendNode(node) => {
                dom.insert_before(*sibling, node);
                self.note_depth(&dom, node);
            }
            NodeOrText::AppendText(text) => {
                let previous = dom.nodes[*sibling].previous;
                if !dom.extend_text(previous, &text) {
                    let node = dom.push(Data::Text(text));
                    dom.insert_before(*sibling, node);
                }
            }
        }
    }

    fn add_attrs_if_missing(&self, target: &Id, new: Vec<Attribute>) {
        if let Data::Element { attrs, .. } = &mut self.dom.borrow_mut().nodes[*target].data {
            for attr in new {
                if !attrs.iter().any(|a| a.name == attr.name) {
                    attrs.push(attr);
                }
            }
        }
    }

    fn remove_from_parent(&self, target: &Id) {
        self.dom.borrow_mut().detach(*target);
    }

    fn reparent_children(&self, node: &Id, new_parent: &Id) {
        let mut dom = self.dom.borrow_mut();
        let mut child = dom.nodes[*node].first_child;
        while let Some(id) = child {
            child = dom.nodes[id].next;
            dom.append(*new_parent, id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tree as `<name>` / `"text"` lines, indented by depth.
    fn outline(dom: &Dom) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![(DOCUMENT, 0)];
        while let Some((id, depth)) = stack.pop() {
            match &dom.nodes[id].data {
                Data::Element { name, .. } => {
                    out.push(format!("{}<{}>", "  ".repeat(depth), name.local))
                }
                Data::Text(text) => out.push(format!("{}{:?}", "  ".repeat(depth), &**text)),
                _ => {}
            }
            let children: Vec<Id> = dom.children(id).collect();
            for child in children.into_iter().rev() {
                stack.push((child, depth + 1));
            }
        }
        out
    }

    #[test]
    fn tag_soup_becomes_a_tree() {
        let dom = Dom::parse("<P>One<BR>two<p>Three <b>bold <i>both</b> italic</i>").unwrap();
        assert_eq!(
            outline(&dom),
            [
                "  <html>",
                "    <head>",
                "    <body>",
                "      <p>",
                "        \"One\"",
                "        <br>",
                "        \"two\"",
                "      <p>",
                "        \"Three \"",
                "        <b>",
                "          \"bold \"",
                "          <i>",
                "            \"both\"",
                "        <i>",
                "          \" italic\"",
            ]
        );
    }

    #[test]
    fn foster_parenting_and_templates() {
        let dom =
            Dom::parse("<table>x<tr><td>cell</td></tr>y</table><template><p>t</p></template>")
                .unwrap();
        let lines = outline(&dom);
        let body = lines.iter().position(|l| l.trim() == "<body>").unwrap();
        assert_eq!(lines[body + 1].trim(), "\"xy\"", "{lines:#?}");
        assert!(
            !lines.iter().any(|l| l.contains("\"t\"")),
            "template content is not in the tree"
        );
    }

    #[test]
    fn attributes_and_lookup() {
        let dom = Dom::parse("<div id=a class=b><a name=x href='#y'>l</a></div>").unwrap();
        let a = dom.find("a").unwrap();
        assert_eq!(dom.attr(a, "name"), Some("x"));
        assert_eq!(dom.attr(a, "href"), Some("#y"));
        assert_eq!(dom.attr(dom.find("div").unwrap(), "id"), Some("a"));
        assert_eq!(dom.attr(a, "missing"), None);
    }

    #[test]
    fn deep_nesting_stops_parsing_early() {
        let start = std::time::Instant::now();
        assert_eq!(
            Dom::parse(&"<div>".repeat(20_000)).err(),
            Some(TooBig::Deep)
        );
        assert_eq!(
            Dom::parse(&"<b><i>".repeat(10_000)).err(),
            Some(TooBig::Deep)
        );
        assert!(start.elapsed().as_secs() < 20, "{:?}", start.elapsed());
        let dom = Dom::parse(&"<div>".repeat(MAX_DEPTH as usize - 5)).unwrap();
        assert!(dom.descendants(DOCUMENT).count() > MAX_DEPTH as usize - 5);
    }

    #[test]
    fn chunks_split_at_character_boundaries() {
        let text = format!("<p>{}</p>", "ж".repeat(100_000));
        let dom = Dom::parse(&text).unwrap();
        let p = dom.find("p").unwrap();
        let child = dom.children(p).next().unwrap();
        match &dom.nodes[child].data {
            Data::Text(t) => assert_eq!(t.chars().count(), 100_000),
            other => panic!("{other:?}"),
        }
    }
}
