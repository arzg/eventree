mod tag;

use self::tag::Tag;
use crate::{SyntaxKind, SyntaxNode, SyntaxToken};
use std::marker::PhantomData;
use std::slice;
use std::sync::atomic::{AtomicU32, Ordering};
use text_size::TextRange;

/// `SyntaxTree` owns the syntax tree allocation.
/// To construct a tree, see [`SyntaxBuilder`].
/// To access its contents, see [`SyntaxTree::root`].
///
/// `SyntaxTree`, like all other `Syntax*` types, is generic over `K`
/// which must implement [`SyntaxKind`].
/// This type parameter allows the kinds of nodes and tokens
/// to be converted between a raw concrete type and a custom enum.
///
/// # Format
///
/// The in-memory format of the syntax tree as described below
/// is subject to change and an implementation detail.
///
/// The tree has four sections:
///
/// - `u32` ID
/// - `u32` length of text
/// - `[u8]` UTF-8 encoded text
/// - `[u8]` events
///
/// These are stored contiguously in one memory allocation.
/// Nodes and tokens are a `u32` index into this allocation.
/// All numerical types are stored in the target platform’s native endianness.
///
/// ## ID
///
/// To ensure nodes and tokens are only used with the tree they were created from,
/// every tree is assigned a `u32` ID from an atomic global counter.
/// Nodes and tokens both store the ID of their tree,
/// which is checked when node or token data is accessed.
///
/// ## Text
///
/// The text of the entire source file must be provided upfront,
/// allowing it to be stored efficiently all in one place.
/// This makes getting the text of nodes and tokens incredibly cheap:
/// we can just index into the text section of the tree
/// using the range of the node or token.
///
/// ## Events
///
/// Following the name of this library,
/// the tree is stored as a flat sequence of events.
/// The encoding is as follows:
///
/// - *start node* (14 bytes):
///   - `u16` tag
///   - `u32` index of corresponding *finish node* event
///   - `u32` range start
///   - `u32` range end
/// - *add token* (10 bytes):
///   - `u16` tag
///   - `u32` range start
///   - `u32` range end
/// - *finish node* (2 bytes):
///   - `u16` tag
///
/// ### Tag
///
/// Simplistically, the tag is the following type,
/// but packed into a single `u16`.
///
/// ```
/// # type Kind = u16;
/// enum Tag { StartNode(Kind), AddToken(Kind), FinishNode }
/// ```
///
/// A value of `u16::MAX` indicates a *finish node* event.
/// Any other value could either indicate *start node* or *add token*.
/// These two are distinguished by the highest bit:
/// `1` means *start node*, and `0` means *add token*.
/// The remaining fifteen bits store the kind.
///
/// The highest allowed kind is **not** `0b0111_1111_1111_1111` as one might suspect.
/// Due to `u16::MAX` being dedicated to *finish node*,
/// we must prohibit a kind of fifteen 1s to avoid ambiguity.
/// Thus, the highest allowed kind is `0b0111_1111_1111_1110`.
pub struct SyntaxTree<K> {
    data: Box<[u8]>,
    phantom: PhantomData<K>,
}

/// This type is used to construct a [`SyntaxTree`].
///
/// Due to the custom in-memory format used for [`SyntaxTree`],
/// the text of your entire input must be provided up-front in [`SyntaxBuilder::new`].
pub struct SyntaxBuilder<K> {
    data: Vec<u8>,
    is_root_set: bool,
    current_len: u32,
    start_node_idxs: Vec<usize>,
    nesting: u32,
    phantom: PhantomData<K>,
}

pub(crate) const START_NODE_SIZE: u32 = 2 + 4 + 4 + 4;
pub(crate) const ADD_TOKEN_SIZE: u32 = 2 + 4 + 4;
pub(crate) const FINISH_NODE_SIZE: u32 = 2;

const FINISH_NODE_IDX_PLACEHOLDER: u32 = 0;

static CURRENT_TREE_ID: AtomicU32 = AtomicU32::new(0);

impl<K: SyntaxKind> SyntaxBuilder<K> {
    /// Constructs a new empty `SyntaxBuilder` with the provided source text.
    pub fn new(text: &str) -> Self {
        debug_assert!(K::LAST <= Tag::MAX_KIND);
        assert!(text.len() < u32::MAX as usize);

        let id = CURRENT_TREE_ID.fetch_add(1, Ordering::SeqCst);

        let mut data = id.to_ne_bytes().to_vec();
        data.extend_from_slice(&(text.len() as u32).to_ne_bytes());
        data.extend_from_slice(text.as_bytes());

        Self {
            data,
            is_root_set: false,
            current_len: 0,
            start_node_idxs: Vec::new(),
            nesting: 0,
            phantom: PhantomData,
        }
    }

    /// Starts a new node with the specified kind.
    ///
    /// # Panics
    ///
    /// - if you have finished creating a root node and try to create another
    pub fn start_node(&mut self, kind: K) {
        if self.is_root_set {
            assert_ne!(self.nesting, 0, "root node already created");
        } else {
            self.is_root_set = true;
        }

        self.nesting += 1;

        self.start_node_idxs.push(self.data.len());

        self.data.reserve(START_NODE_SIZE as usize);
        unsafe {
            let ptr = self.data_end_ptr();
            (ptr as *mut Tag).write_unaligned(Tag::start_node(kind));
            (ptr.add(2) as *mut u32).write_unaligned(FINISH_NODE_IDX_PLACEHOLDER);
            (ptr.add(6) as *mut u32).write_unaligned(self.current_len);
            (ptr.add(10) as *mut u32).write_unaligned(self.current_len);
            self.data.set_len(self.data.len() + START_NODE_SIZE as usize);
        }
    }

    /// Adds a token with the provided kind and range to the current node.
    ///
    /// # Panics
    ///
    /// - if you try to add a token before starting a node
    /// - if the provided range is out of bounds of the original input text
    pub fn add_token(&mut self, kind: K, range: TextRange) {
        assert!(self.nesting > 0, "cannot add token before starting node");
        assert!(
            u32::from(range.end()) <= self.text_len(),
            "token is out of range: range is {range:?}, but text is 0..{}",
            self.text_len()
        );

        let start = u32::from(range.start());
        let end = u32::from(range.end());
        self.current_len = end;

        self.data.reserve(ADD_TOKEN_SIZE as usize);
        unsafe {
            let ptr = self.data_end_ptr();
            (ptr as *mut Tag).write_unaligned(Tag::add_token(kind));
            (ptr.add(2) as *mut u32).write_unaligned(start);
            (ptr.add(6) as *mut u32).write_unaligned(end);
            self.data.set_len(self.data.len() + ADD_TOKEN_SIZE as usize);
        }
    }

    /// Completes the current node and makes the parent node current.
    ///
    /// # Panics
    ///
    /// - if all outstanding nodes have already been finished
    pub fn finish_node(&mut self) {
        assert!(self.nesting > 0, "no nodes are yet to be finished");
        self.nesting -= 1;

        let start_node_idx = self.start_node_idxs.pop().unwrap();
        let finish_node_idx = self.data.len() as u32;

        self.data.reserve(FINISH_NODE_SIZE as usize);
        unsafe {
            let ptr = self.data_end_ptr() as *mut Tag;
            ptr.write_unaligned(Tag::finish_node());
            self.data.set_len(self.data.len() + FINISH_NODE_SIZE as usize);
        }

        unsafe {
            let ptr = self.data.as_mut_ptr().add(start_node_idx);
            debug_assert!((ptr as *const Tag).read_unaligned().is_start_node());

            debug_assert_eq!(
                (ptr.add(2) as *const u32).read_unaligned(),
                FINISH_NODE_IDX_PLACEHOLDER
            );
            (ptr.add(2) as *mut u32).write_unaligned(finish_node_idx);

            (ptr.add(10) as *mut u32).write_unaligned(self.current_len);
        }
    }

    /// Completes the tree and freezes it into the read-only [`SyntaxTree`] type.
    ///
    /// # Panics
    ///
    /// - if no nodes have been created
    /// - if there are nodes which have not been finished
    pub fn finish(self) -> SyntaxTree<K> {
        let Self { data, is_root_set, current_len: _, start_node_idxs: _, nesting, phantom: _ } =
            self;

        assert!(is_root_set, "no nodes created");

        assert_eq!(nesting, 0, "did not finish all nodes ({nesting} unfinished nodes)");

        // into_boxed_slice calls shrink_to_fit for us
        SyntaxTree { data: data.into_boxed_slice(), phantom: PhantomData }
    }

    fn text_len(&self) -> u32 {
        unsafe { (self.data.as_ptr() as *const u32).add(1).read_unaligned() }
    }

    fn data_end_ptr(&mut self) -> *mut u8 {
        unsafe { self.data.as_mut_ptr().add(self.data.len()) }
    }
}

impl<K: SyntaxKind> SyntaxTree<K> {
    /// Returns the root node of this tree.
    pub fn root(&self) -> SyntaxNode<K> {
        SyntaxNode::new(self.root_idx(), self.id())
    }

    pub(crate) fn root_idx(&self) -> u32 {
        let text_len = unsafe { (self.data.as_ptr() as *const u32).add(1).read_unaligned() };
        text_len + 8
    }

    pub(crate) fn id(&self) -> u32 {
        unsafe { (self.data.as_ptr() as *const u32).read_unaligned() }
    }

    pub(crate) unsafe fn get_text(&self, start: u32, end: u32) -> &str {
        let start = start as usize + 8;
        let end = end as usize + 8;

        let slice = slice::from_raw_parts(self.data.as_ptr().add(start), end - start);

        if cfg!(debug_assertions) {
            std::str::from_utf8(slice).unwrap()
        } else {
            std::str::from_utf8_unchecked(slice)
        }
    }

    pub(crate) unsafe fn get_start_node(&self, idx: u32) -> (K, u32, u32, u32) {
        let idx = idx as usize;
        debug_assert!(idx + START_NODE_SIZE as usize <= self.data.len());

        let ptr = self.data.as_ptr().add(idx);
        let tag = (ptr as *const Tag).read_unaligned();
        let finish_node_idx = (ptr.add(2) as *const u32).read_unaligned();
        let start = (ptr.add(6) as *const u32).read_unaligned();
        let end = (ptr.add(10) as *const u32).read_unaligned();

        let kind = tag.get_start_node_kind();

        (kind, finish_node_idx, start, end)
    }

    pub(crate) unsafe fn get_add_token(&self, idx: u32) -> (K, u32, u32) {
        let idx = idx as usize;
        debug_assert!(idx + ADD_TOKEN_SIZE as usize <= self.data.len());

        let ptr = self.data.as_ptr().add(idx);
        let tag = (ptr as *const Tag).read_unaligned();
        let start = (ptr.add(2) as *const u32).read_unaligned();
        let end = (ptr.add(6) as *const u32).read_unaligned();

        let kind = tag.get_add_token_kind();

        (kind, start, end)
    }

    pub(crate) unsafe fn is_start_node(&self, idx: u32) -> bool {
        self.tag_at_idx(idx).is_start_node()
    }

    pub(crate) unsafe fn is_add_token(&self, idx: u32) -> bool {
        self.tag_at_idx(idx).is_add_token()
    }

    pub(crate) unsafe fn is_finish_node(&self, idx: u32) -> bool {
        self.tag_at_idx(idx).is_finish_node()
    }

    fn tag_at_idx(&self, idx: u32) -> Tag {
        let idx = idx as usize;
        debug_assert!(idx < self.data.len());
        unsafe { (self.data.as_ptr().add(idx) as *const Tag).read_unaligned() }
    }
}

impl<K: SyntaxKind> std::fmt::Debug for SyntaxTree<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !f.alternate() {
            return f.debug_struct("SyntaxTree").field("data", &self.data).finish();
        }

        let mut indentation_level = 0_usize;

        let mut idx = self.root_idx();
        while idx < self.data.len() as u32 {
            if unsafe { self.is_finish_node(idx) } {
                indentation_level -= 1;
                idx += FINISH_NODE_SIZE;
                continue;
            }

            for _ in 0..indentation_level {
                write!(f, "  ")?;
            }

            if unsafe { self.is_start_node(idx) } {
                let node = SyntaxNode::new(idx, self.id());
                let kind = node.kind(self);
                let range = node.range(self);
                writeln!(f, "{kind:?}@{range:?}")?;
                indentation_level += 1;
                idx += START_NODE_SIZE;
                continue;
            }

            if unsafe { self.is_add_token(idx) } {
                let token = SyntaxToken::new(idx, self.id());
                let kind = token.kind(self);
                let text = token.text(self);
                let range = token.range(self);
                writeln!(f, "{kind:?}@{range:?} {text:?}")?;
                idx += ADD_TOKEN_SIZE;
                continue;
            }

            unreachable!()
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use expect_test::expect;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    #[repr(u16)]
    enum SyntaxKind {
        Root,
        Arrow,
        Block,
        Comment,
        FncKw,
        Function,
        Ident,
        LBrace,
        LetKw,
        RBrace,
        Semicolon,
        __Last,
    }

    unsafe impl crate::SyntaxKind for SyntaxKind {
        const LAST: u16 = Self::__Last as u16;

        fn to_raw(self) -> u16 {
            self as u16
        }

        unsafe fn from_raw(raw: u16) -> Self {
            std::mem::transmute(raw)
        }
    }

    enum D {
        U16(u16),
        U32(u32),
        Text(&'static str),
    }

    fn check<const N: usize>(
        input: &str,
        f: impl Fn(&mut SyntaxBuilder<SyntaxKind>),
        data: [D; N],
    ) {
        let mut builder = SyntaxBuilder::new(input);
        f(&mut builder);
        let tree = builder.finish();

        let data: Vec<_> = data
            .into_iter()
            .flat_map(|num| match num {
                D::U16(n) => n.to_ne_bytes().to_vec(),
                D::U32(n) => n.to_ne_bytes().to_vec(),
                D::Text(s) => s.as_bytes().to_vec(),
            })
            .collect();

        // don’t include tag in tests
        assert_eq!(tree.data[4..], data);
    }

    #[test]
    fn just_root() {
        check(
            "",
            |b| {
                b.start_node(SyntaxKind::Root);
                b.finish_node();
            },
            [
                D::U32(0),
                D::U16(SyntaxKind::Root as u16 | 1 << 15),
                D::U32(22),
                D::U32(0),
                D::U32(0),
                D::U16(u16::MAX),
            ],
        );
    }

    #[test]
    fn add_token() {
        check(
            "let",
            |b| {
                b.start_node(SyntaxKind::Root);
                b.add_token(SyntaxKind::LetKw, TextRange::new(0.into(), 3.into()));
                b.finish_node();
            },
            [
                D::U32(3),
                D::Text("let"),
                D::U16(SyntaxKind::Root as u16 | 1 << 15),
                D::U32(35),
                D::U32(0),
                D::U32(3),
                D::U16(SyntaxKind::LetKw as u16),
                D::U32(0),
                D::U32(3),
                D::U16(u16::MAX),
            ],
        );
    }

    #[test]
    fn debug_empty() {
        let mut builder = SyntaxBuilder::new("");
        builder.start_node(SyntaxKind::Root);
        builder.finish_node();

        let tree = builder.finish();
        expect![[r##"
            Root@0..0
        "##]]
        .assert_eq(&format!("{tree:#?}"));
    }

    #[test]
    fn debug_complex() {
        let mut builder = SyntaxBuilder::new("# foo\nfncbar->{};");
        builder.start_node(SyntaxKind::Root);
        builder.add_token(SyntaxKind::Comment, TextRange::new(0.into(), 6.into()));
        builder.start_node(SyntaxKind::Function);
        builder.add_token(SyntaxKind::FncKw, TextRange::new(6.into(), 9.into()));
        builder.add_token(SyntaxKind::Ident, TextRange::new(9.into(), 12.into()));
        builder.add_token(SyntaxKind::Arrow, TextRange::new(12.into(), 14.into()));
        builder.start_node(SyntaxKind::Block);
        builder.add_token(SyntaxKind::LBrace, TextRange::new(14.into(), 15.into()));
        builder.add_token(SyntaxKind::RBrace, TextRange::new(15.into(), 16.into()));
        builder.finish_node();
        builder.add_token(SyntaxKind::Semicolon, TextRange::new(16.into(), 17.into()));
        builder.finish_node();
        builder.finish_node();

        let tree = builder.finish();
        expect![[r##"
            Root@0..17
              Comment@0..6 "# foo\n"
              Function@6..17
                FncKw@6..9 "fnc"
                Ident@9..12 "bar"
                Arrow@12..14 "->"
                Block@14..16
                  LBrace@14..15 "{"
                  RBrace@15..16 "}"
                Semicolon@16..17 ";"
        "##]]
        .assert_eq(&format!("{tree:#?}"));
    }

    #[test]
    #[should_panic(expected = "no nodes are yet to be finished")]
    fn no_start_node() {
        let mut builder = SyntaxBuilder::<SyntaxKind>::new("");
        builder.finish_node();
    }

    #[test]
    #[should_panic(expected = "did not finish all nodes (1 unfinished nodes)")]
    fn no_finish_node() {
        let mut builder = SyntaxBuilder::new("");
        builder.start_node(SyntaxKind::Root);
        builder.finish();
    }

    #[test]
    #[should_panic(expected = "did not finish all nodes (2 unfinished nodes)")]
    fn too_many_start_node_calls() {
        let mut builder = SyntaxBuilder::new("");
        builder.start_node(SyntaxKind::Root);
        builder.start_node(SyntaxKind::Function);
        builder.start_node(SyntaxKind::Block);
        builder.start_node(SyntaxKind::Block);
        builder.finish_node();
        builder.finish_node();
        builder.finish();
    }

    #[test]
    #[should_panic(expected = "no nodes are yet to be finished")]
    fn too_many_finish_node_calls() {
        let mut builder = SyntaxBuilder::new("");
        builder.start_node(SyntaxKind::Root);
        builder.start_node(SyntaxKind::Function);
        builder.start_node(SyntaxKind::Block);
        builder.finish_node();
        builder.finish_node();
        builder.finish_node();
        builder.finish_node();
    }

    #[test]
    #[should_panic(expected = "root node already created")]
    fn second_root() {
        let mut builder = SyntaxBuilder::new("");
        builder.start_node(SyntaxKind::Root);
        builder.finish_node();
        builder.start_node(SyntaxKind::Block);
    }

    #[test]
    #[should_panic(expected = "no nodes created")]
    fn empty_without_text() {
        SyntaxBuilder::<SyntaxKind>::new("").finish();
    }

    #[test]
    #[should_panic(expected = "no nodes created")]
    fn empty_with_text() {
        SyntaxBuilder::<SyntaxKind>::new("foo").finish();
    }

    #[test]
    #[should_panic(expected = "cannot add token before starting node")]
    fn add_token_before_starting_node() {
        let mut builder = SyntaxBuilder::new("let");
        builder.add_token(SyntaxKind::LetKw, TextRange::new(0.into(), 3.into()));
    }

    #[test]
    #[should_panic(expected = "token is out of range: range is 0..1, but text is 0..0")]
    fn add_token_with_out_of_bounds_range() {
        let mut builder = SyntaxBuilder::new("");
        builder.start_node(SyntaxKind::Root);
        builder.add_token(SyntaxKind::LetKw, TextRange::new(0.into(), 1.into()));
    }

    #[test]
    #[should_panic(
        expected = "tried to access node data from tree other than the one this node is from"
    )]
    fn access_node_data_from_other_tree() {
        let mut builder = SyntaxBuilder::new("");
        builder.start_node(SyntaxKind::Root);
        builder.finish_node();
        let tree = builder.finish();

        let mut builder = SyntaxBuilder::new("");
        builder.start_node(SyntaxKind::Root);
        builder.finish_node();
        let tree2 = builder.finish();

        tree.root().text(&tree2);
    }

    #[test]
    #[should_panic(
        expected = "tried to access token data from tree other than the one this token is from"
    )]
    fn access_token_data_from_other_tree() {
        let mut builder = SyntaxBuilder::new("->");
        builder.start_node(SyntaxKind::Root);
        builder.add_token(SyntaxKind::Arrow, TextRange::new(0.into(), 2.into()));
        builder.finish_node();
        let tree = builder.finish();

        let mut builder = SyntaxBuilder::new("");
        builder.start_node(SyntaxKind::Root);
        builder.finish_node();
        let tree2 = builder.finish();

        let arrow_token = tree.root().child_tokens(&tree).next().unwrap();
        arrow_token.text(&tree2);
    }
}
