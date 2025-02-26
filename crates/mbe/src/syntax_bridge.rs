//! Conversions between [`SyntaxNode`] and [`tt::TokenTree`].

use rustc_hash::FxHashMap;
use stdx::{always, non_empty_vec::NonEmptyVec};
use syntax::{
    ast::{self, make::tokens::doc_comment},
    AstToken, Parse, PreorderWithTokens, SmolStr, SyntaxElement, SyntaxKind,
    SyntaxKind::*,
    SyntaxNode, SyntaxToken, SyntaxTreeBuilder, TextRange, TextSize, WalkEvent, T,
};
use tt::buffer::{Cursor, TokenBuffer};

use crate::{to_parser_input::to_parser_input, tt_iter::TtIter, TokenMap};

/// Convert the syntax node to a `TokenTree` (what macro
/// will consume).
pub fn syntax_node_to_token_tree(node: &SyntaxNode) -> (tt::Subtree, TokenMap) {
    let (subtree, token_map, _) = syntax_node_to_token_tree_with_modifications(
        node,
        Default::default(),
        0,
        Default::default(),
        Default::default(),
    );
    (subtree, token_map)
}

/// Convert the syntax node to a `TokenTree` (what macro will consume)
/// with the censored range excluded.
pub fn syntax_node_to_token_tree_with_modifications(
    node: &SyntaxNode,
    existing_token_map: TokenMap,
    next_id: u32,
    replace: FxHashMap<SyntaxNode, Vec<SyntheticToken>>,
    append: FxHashMap<SyntaxNode, Vec<SyntheticToken>>,
) -> (tt::Subtree, TokenMap, u32) {
    let global_offset = node.text_range().start();
    let mut c = Convertor::new(node, global_offset, existing_token_map, next_id, replace, append);
    let subtree = convert_tokens(&mut c);
    c.id_alloc.map.shrink_to_fit();
    always!(c.replace.is_empty(), "replace: {:?}", c.replace);
    always!(c.append.is_empty(), "append: {:?}", c.append);
    (subtree, c.id_alloc.map, c.id_alloc.next_id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SyntheticTokenId(pub u32);

#[derive(Debug, Clone)]
pub struct SyntheticToken {
    pub kind: SyntaxKind,
    pub text: SmolStr,
    pub range: TextRange,
    pub id: SyntheticTokenId,
}

// The following items are what `rustc` macro can be parsed into :
// link: https://github.com/rust-lang/rust/blob/9ebf47851a357faa4cd97f4b1dc7835f6376e639/src/libsyntax/ext/expand.rs#L141
// * Expr(P<ast::Expr>)                     -> token_tree_to_expr
// * Pat(P<ast::Pat>)                       -> token_tree_to_pat
// * Ty(P<ast::Ty>)                         -> token_tree_to_ty
// * Stmts(SmallVec<[ast::Stmt; 1]>)        -> token_tree_to_stmts
// * Items(SmallVec<[P<ast::Item>; 1]>)     -> token_tree_to_items
//
// * TraitItems(SmallVec<[ast::TraitItem; 1]>)
// * AssocItems(SmallVec<[ast::AssocItem; 1]>)
// * ForeignItems(SmallVec<[ast::ForeignItem; 1]>

pub fn token_tree_to_syntax_node(
    tt: &tt::Subtree,
    entry_point: parser::TopEntryPoint,
) -> (Parse<SyntaxNode>, TokenMap) {
    let buffer = match tt {
        tt::Subtree { delimiter: None, token_trees } => {
            TokenBuffer::from_tokens(token_trees.as_slice())
        }
        _ => TokenBuffer::from_subtree(tt),
    };
    let parser_input = to_parser_input(&buffer);
    let parser_output = entry_point.parse(&parser_input);
    let mut tree_sink = TtTreeSink::new(buffer.begin());
    for event in parser_output.iter() {
        match event {
            parser::Step::Token { kind, n_input_tokens: n_raw_tokens } => {
                tree_sink.token(kind, n_raw_tokens)
            }
            parser::Step::Enter { kind } => tree_sink.start_node(kind),
            parser::Step::Exit => tree_sink.finish_node(),
            parser::Step::Error { msg } => tree_sink.error(msg.to_string()),
        }
    }
    let (parse, range_map) = tree_sink.finish();
    (parse, range_map)
}

/// Convert a string to a `TokenTree`
pub fn parse_to_token_tree(text: &str) -> Option<(tt::Subtree, TokenMap)> {
    let lexed = parser::LexedStr::new(text);
    if lexed.errors().next().is_some() {
        return None;
    }

    let mut conv = RawConvertor {
        lexed,
        pos: 0,
        id_alloc: TokenIdAlloc {
            map: Default::default(),
            global_offset: TextSize::default(),
            next_id: 0,
        },
    };

    let subtree = convert_tokens(&mut conv);
    Some((subtree, conv.id_alloc.map))
}

/// Split token tree with separate expr: $($e:expr)SEP*
pub fn parse_exprs_with_sep(tt: &tt::Subtree, sep: char) -> Vec<tt::Subtree> {
    if tt.token_trees.is_empty() {
        return Vec::new();
    }

    let mut iter = TtIter::new(tt);
    let mut res = Vec::new();

    while iter.peek_n(0).is_some() {
        let expanded = iter.expect_fragment(parser::PrefixEntryPoint::Expr);

        res.push(match expanded.value {
            None => break,
            Some(tt @ tt::TokenTree::Leaf(_)) => {
                tt::Subtree { delimiter: None, token_trees: vec![tt] }
            }
            Some(tt::TokenTree::Subtree(tt)) => tt,
        });

        let mut fork = iter.clone();
        if fork.expect_char(sep).is_err() {
            break;
        }
        iter = fork;
    }

    if iter.peek_n(0).is_some() {
        res.push(tt::Subtree { delimiter: None, token_trees: iter.into_iter().cloned().collect() });
    }

    res
}

fn convert_tokens<C: TokenConvertor>(conv: &mut C) -> tt::Subtree {
    struct StackEntry {
        subtree: tt::Subtree,
        idx: usize,
        open_range: TextRange,
    }

    let entry = StackEntry {
        subtree: tt::Subtree { delimiter: None, ..Default::default() },
        // never used (delimiter is `None`)
        idx: !0,
        open_range: TextRange::empty(TextSize::of('.')),
    };
    let mut stack = NonEmptyVec::new(entry);

    loop {
        let StackEntry { subtree, .. } = stack.last_mut();
        let result = &mut subtree.token_trees;
        let (token, range) = match conv.bump() {
            Some(it) => it,
            None => break,
        };
        let synth_id = token.synthetic_id(&conv);

        let kind = token.kind(&conv);
        if kind == COMMENT {
            if let Some(tokens) = conv.convert_doc_comment(&token) {
                // FIXME: There has to be a better way to do this
                // Add the comments token id to the converted doc string
                let id = conv.id_alloc().alloc(range, synth_id);
                result.extend(tokens.into_iter().map(|mut tt| {
                    if let tt::TokenTree::Subtree(sub) = &mut tt {
                        if let Some(tt::TokenTree::Leaf(tt::Leaf::Literal(lit))) =
                            sub.token_trees.get_mut(2)
                        {
                            lit.id = id
                        }
                    }
                    tt
                }));
            }
            continue;
        }
        let tt = if kind.is_punct() && kind != UNDERSCORE {
            if synth_id.is_none() {
                assert_eq!(range.len(), TextSize::of('.'));
            }

            if let Some(delim) = subtree.delimiter {
                let expected = match delim.kind {
                    tt::DelimiterKind::Parenthesis => T![')'],
                    tt::DelimiterKind::Brace => T!['}'],
                    tt::DelimiterKind::Bracket => T![']'],
                };

                if kind == expected {
                    if let Some(entry) = stack.pop() {
                        conv.id_alloc().close_delim(entry.idx, Some(range));
                        stack.last_mut().subtree.token_trees.push(entry.subtree.into());
                    }
                    continue;
                }
            }

            let delim = match kind {
                T!['('] => Some(tt::DelimiterKind::Parenthesis),
                T!['{'] => Some(tt::DelimiterKind::Brace),
                T!['['] => Some(tt::DelimiterKind::Bracket),
                _ => None,
            };

            if let Some(kind) = delim {
                let mut subtree = tt::Subtree::default();
                let (id, idx) = conv.id_alloc().open_delim(range);
                subtree.delimiter = Some(tt::Delimiter { id, kind });
                stack.push(StackEntry { subtree, idx, open_range: range });
                continue;
            }

            let spacing = match conv.peek().map(|next| next.kind(&conv)) {
                Some(kind)
                    if !kind.is_trivia()
                        && kind.is_punct()
                        && kind != T!['[']
                        && kind != T!['{']
                        && kind != T!['(']
                        && kind != UNDERSCORE =>
                {
                    tt::Spacing::Joint
                }
                _ => tt::Spacing::Alone,
            };
            let char = match token.to_char(&conv) {
                Some(c) => c,
                None => {
                    panic!("Token from lexer must be single char: token = {:#?}", token);
                }
            };
            tt::Leaf::from(tt::Punct { char, spacing, id: conv.id_alloc().alloc(range, synth_id) })
                .into()
        } else {
            macro_rules! make_leaf {
                ($i:ident) => {
                    tt::$i { id: conv.id_alloc().alloc(range, synth_id), text: token.to_text(conv) }
                        .into()
                };
            }
            let leaf: tt::Leaf = match kind {
                T![true] | T![false] => make_leaf!(Ident),
                IDENT => make_leaf!(Ident),
                UNDERSCORE => make_leaf!(Ident),
                k if k.is_keyword() => make_leaf!(Ident),
                k if k.is_literal() => make_leaf!(Literal),
                LIFETIME_IDENT => {
                    let char_unit = TextSize::of('\'');
                    let r = TextRange::at(range.start(), char_unit);
                    let apostrophe = tt::Leaf::from(tt::Punct {
                        char: '\'',
                        spacing: tt::Spacing::Joint,
                        id: conv.id_alloc().alloc(r, synth_id),
                    });
                    result.push(apostrophe.into());

                    let r = TextRange::at(range.start() + char_unit, range.len() - char_unit);
                    let ident = tt::Leaf::from(tt::Ident {
                        text: SmolStr::new(&token.to_text(conv)[1..]),
                        id: conv.id_alloc().alloc(r, synth_id),
                    });
                    result.push(ident.into());
                    continue;
                }
                _ => continue,
            };

            leaf.into()
        };
        result.push(tt);
    }

    // If we get here, we've consumed all input tokens.
    // We might have more than one subtree in the stack, if the delimiters are improperly balanced.
    // Merge them so we're left with one.
    while let Some(entry) = stack.pop() {
        let parent = stack.last_mut();

        conv.id_alloc().close_delim(entry.idx, None);
        let leaf: tt::Leaf = tt::Punct {
            id: conv.id_alloc().alloc(entry.open_range, None),
            char: match entry.subtree.delimiter.unwrap().kind {
                tt::DelimiterKind::Parenthesis => '(',
                tt::DelimiterKind::Brace => '{',
                tt::DelimiterKind::Bracket => '[',
            },
            spacing: tt::Spacing::Alone,
        }
        .into();
        parent.subtree.token_trees.push(leaf.into());
        parent.subtree.token_trees.extend(entry.subtree.token_trees);
    }

    let subtree = stack.into_last().subtree;
    if let [tt::TokenTree::Subtree(first)] = &*subtree.token_trees {
        first.clone()
    } else {
        subtree
    }
}

/// Returns the textual content of a doc comment block as a quoted string
/// That is, strips leading `///` (or `/**`, etc)
/// and strips the ending `*/`
/// And then quote the string, which is needed to convert to `tt::Literal`
fn doc_comment_text(comment: &ast::Comment) -> SmolStr {
    let prefix_len = comment.prefix().len();
    let mut text = &comment.text()[prefix_len..];

    // Remove ending "*/"
    if comment.kind().shape == ast::CommentShape::Block {
        text = &text[0..text.len() - 2];
    }

    // Quote the string
    // Note that `tt::Literal` expect an escaped string
    let text = format!("\"{}\"", text.escape_debug());
    text.into()
}

fn convert_doc_comment(token: &syntax::SyntaxToken) -> Option<Vec<tt::TokenTree>> {
    cov_mark::hit!(test_meta_doc_comments);
    let comment = ast::Comment::cast(token.clone())?;
    let doc = comment.kind().doc?;

    // Make `doc="\" Comments\""
    let meta_tkns = vec![mk_ident("doc"), mk_punct('='), mk_doc_literal(&comment)];

    // Make `#![]`
    let mut token_trees = Vec::with_capacity(3);
    token_trees.push(mk_punct('#'));
    if let ast::CommentPlacement::Inner = doc {
        token_trees.push(mk_punct('!'));
    }
    token_trees.push(tt::TokenTree::from(tt::Subtree {
        delimiter: Some(tt::Delimiter {
            kind: tt::DelimiterKind::Bracket,
            id: tt::TokenId::unspecified(),
        }),
        token_trees: meta_tkns,
    }));

    return Some(token_trees);

    // Helper functions
    fn mk_ident(s: &str) -> tt::TokenTree {
        tt::TokenTree::from(tt::Leaf::from(tt::Ident {
            text: s.into(),
            id: tt::TokenId::unspecified(),
        }))
    }

    fn mk_punct(c: char) -> tt::TokenTree {
        tt::TokenTree::from(tt::Leaf::from(tt::Punct {
            char: c,
            spacing: tt::Spacing::Alone,
            id: tt::TokenId::unspecified(),
        }))
    }

    fn mk_doc_literal(comment: &ast::Comment) -> tt::TokenTree {
        let lit = tt::Literal { text: doc_comment_text(comment), id: tt::TokenId::unspecified() };

        tt::TokenTree::from(tt::Leaf::from(lit))
    }
}

struct TokenIdAlloc {
    map: TokenMap,
    global_offset: TextSize,
    next_id: u32,
}

impl TokenIdAlloc {
    fn alloc(
        &mut self,
        absolute_range: TextRange,
        synthetic_id: Option<SyntheticTokenId>,
    ) -> tt::TokenId {
        let relative_range = absolute_range - self.global_offset;
        let token_id = tt::TokenId(self.next_id);
        self.next_id += 1;
        self.map.insert(token_id, relative_range);
        if let Some(id) = synthetic_id {
            self.map.insert_synthetic(token_id, id);
        }
        token_id
    }

    fn open_delim(&mut self, open_abs_range: TextRange) -> (tt::TokenId, usize) {
        let token_id = tt::TokenId(self.next_id);
        self.next_id += 1;
        let idx = self.map.insert_delim(
            token_id,
            open_abs_range - self.global_offset,
            open_abs_range - self.global_offset,
        );
        (token_id, idx)
    }

    fn close_delim(&mut self, idx: usize, close_abs_range: Option<TextRange>) {
        match close_abs_range {
            None => {
                self.map.remove_delim(idx);
            }
            Some(close) => {
                self.map.update_close_delim(idx, close - self.global_offset);
            }
        }
    }
}

/// A raw token (straight from lexer) convertor
struct RawConvertor<'a> {
    lexed: parser::LexedStr<'a>,
    pos: usize,
    id_alloc: TokenIdAlloc,
}

trait SrcToken<Ctx>: std::fmt::Debug {
    fn kind(&self, ctx: &Ctx) -> SyntaxKind;

    fn to_char(&self, ctx: &Ctx) -> Option<char>;

    fn to_text(&self, ctx: &Ctx) -> SmolStr;

    fn synthetic_id(&self, ctx: &Ctx) -> Option<SyntheticTokenId>;
}

trait TokenConvertor: Sized {
    type Token: SrcToken<Self>;

    fn convert_doc_comment(&self, token: &Self::Token) -> Option<Vec<tt::TokenTree>>;

    fn bump(&mut self) -> Option<(Self::Token, TextRange)>;

    fn peek(&self) -> Option<Self::Token>;

    fn id_alloc(&mut self) -> &mut TokenIdAlloc;
}

impl<'a> SrcToken<RawConvertor<'a>> for usize {
    fn kind(&self, ctx: &RawConvertor<'a>) -> SyntaxKind {
        ctx.lexed.kind(*self)
    }

    fn to_char(&self, ctx: &RawConvertor<'a>) -> Option<char> {
        ctx.lexed.text(*self).chars().next()
    }

    fn to_text(&self, ctx: &RawConvertor<'_>) -> SmolStr {
        ctx.lexed.text(*self).into()
    }

    fn synthetic_id(&self, _ctx: &RawConvertor<'a>) -> Option<SyntheticTokenId> {
        None
    }
}

impl<'a> TokenConvertor for RawConvertor<'a> {
    type Token = usize;

    fn convert_doc_comment(&self, &token: &usize) -> Option<Vec<tt::TokenTree>> {
        let text = self.lexed.text(token);
        convert_doc_comment(&doc_comment(text))
    }

    fn bump(&mut self) -> Option<(Self::Token, TextRange)> {
        if self.pos == self.lexed.len() {
            return None;
        }
        let token = self.pos;
        self.pos += 1;
        let range = self.lexed.text_range(token);
        let range = TextRange::new(range.start.try_into().unwrap(), range.end.try_into().unwrap());

        Some((token, range))
    }

    fn peek(&self) -> Option<Self::Token> {
        if self.pos == self.lexed.len() {
            return None;
        }
        Some(self.pos)
    }

    fn id_alloc(&mut self) -> &mut TokenIdAlloc {
        &mut self.id_alloc
    }
}

struct Convertor {
    id_alloc: TokenIdAlloc,
    current: Option<SyntaxToken>,
    current_synthetic: Vec<SyntheticToken>,
    preorder: PreorderWithTokens,
    replace: FxHashMap<SyntaxNode, Vec<SyntheticToken>>,
    append: FxHashMap<SyntaxNode, Vec<SyntheticToken>>,
    range: TextRange,
    punct_offset: Option<(SyntaxToken, TextSize)>,
}

impl Convertor {
    fn new(
        node: &SyntaxNode,
        global_offset: TextSize,
        existing_token_map: TokenMap,
        next_id: u32,
        mut replace: FxHashMap<SyntaxNode, Vec<SyntheticToken>>,
        mut append: FxHashMap<SyntaxNode, Vec<SyntheticToken>>,
    ) -> Convertor {
        let range = node.text_range();
        let mut preorder = node.preorder_with_tokens();
        let (first, synthetic) = Self::next_token(&mut preorder, &mut replace, &mut append);
        Convertor {
            id_alloc: { TokenIdAlloc { map: existing_token_map, global_offset, next_id } },
            current: first,
            current_synthetic: synthetic,
            preorder,
            range,
            replace,
            append,
            punct_offset: None,
        }
    }

    fn next_token(
        preorder: &mut PreorderWithTokens,
        replace: &mut FxHashMap<SyntaxNode, Vec<SyntheticToken>>,
        append: &mut FxHashMap<SyntaxNode, Vec<SyntheticToken>>,
    ) -> (Option<SyntaxToken>, Vec<SyntheticToken>) {
        while let Some(ev) = preorder.next() {
            let ele = match ev {
                WalkEvent::Enter(ele) => ele,
                WalkEvent::Leave(SyntaxElement::Node(node)) => {
                    if let Some(mut v) = append.remove(&node) {
                        if !v.is_empty() {
                            v.reverse();
                            return (None, v);
                        }
                    }
                    continue;
                }
                _ => continue,
            };
            match ele {
                SyntaxElement::Token(t) => return (Some(t), Vec::new()),
                SyntaxElement::Node(node) => {
                    if let Some(mut v) = replace.remove(&node) {
                        preorder.skip_subtree();
                        if !v.is_empty() {
                            v.reverse();
                            return (None, v);
                        }
                    }
                }
            }
        }
        (None, Vec::new())
    }
}

#[derive(Debug)]
enum SynToken {
    Ordinary(SyntaxToken),
    // FIXME is this supposed to be `Punct`?
    Punch(SyntaxToken, TextSize),
    Synthetic(SyntheticToken),
}

impl SynToken {
    fn token(&self) -> Option<&SyntaxToken> {
        match self {
            SynToken::Ordinary(it) | SynToken::Punch(it, _) => Some(it),
            SynToken::Synthetic(_) => None,
        }
    }
}

impl SrcToken<Convertor> for SynToken {
    fn kind(&self, _ctx: &Convertor) -> SyntaxKind {
        match self {
            SynToken::Ordinary(token) => token.kind(),
            SynToken::Punch(token, _) => token.kind(),
            SynToken::Synthetic(token) => token.kind,
        }
    }
    fn to_char(&self, _ctx: &Convertor) -> Option<char> {
        match self {
            SynToken::Ordinary(_) => None,
            SynToken::Punch(it, i) => it.text().chars().nth((*i).into()),
            SynToken::Synthetic(token) if token.text.len() == 1 => token.text.chars().next(),
            SynToken::Synthetic(_) => None,
        }
    }
    fn to_text(&self, _ctx: &Convertor) -> SmolStr {
        match self {
            SynToken::Ordinary(token) => token.text().into(),
            SynToken::Punch(token, _) => token.text().into(),
            SynToken::Synthetic(token) => token.text.clone(),
        }
    }

    fn synthetic_id(&self, _ctx: &Convertor) -> Option<SyntheticTokenId> {
        match self {
            SynToken::Synthetic(token) => Some(token.id),
            _ => None,
        }
    }
}

impl TokenConvertor for Convertor {
    type Token = SynToken;
    fn convert_doc_comment(&self, token: &Self::Token) -> Option<Vec<tt::TokenTree>> {
        convert_doc_comment(token.token()?)
    }

    fn bump(&mut self) -> Option<(Self::Token, TextRange)> {
        if let Some((punct, offset)) = self.punct_offset.clone() {
            if usize::from(offset) + 1 < punct.text().len() {
                let offset = offset + TextSize::of('.');
                let range = punct.text_range();
                self.punct_offset = Some((punct.clone(), offset));
                let range = TextRange::at(range.start() + offset, TextSize::of('.'));
                return Some((SynToken::Punch(punct, offset), range));
            }
        }

        if let Some(synth_token) = self.current_synthetic.pop() {
            if self.current_synthetic.is_empty() {
                let (new_current, new_synth) =
                    Self::next_token(&mut self.preorder, &mut self.replace, &mut self.append);
                self.current = new_current;
                self.current_synthetic = new_synth;
            }
            let range = synth_token.range;
            return Some((SynToken::Synthetic(synth_token), range));
        }

        let curr = self.current.clone()?;
        if !&self.range.contains_range(curr.text_range()) {
            return None;
        }
        let (new_current, new_synth) =
            Self::next_token(&mut self.preorder, &mut self.replace, &mut self.append);
        self.current = new_current;
        self.current_synthetic = new_synth;
        let token = if curr.kind().is_punct() {
            self.punct_offset = Some((curr.clone(), 0.into()));
            let range = curr.text_range();
            let range = TextRange::at(range.start(), TextSize::of('.'));
            (SynToken::Punch(curr, 0.into()), range)
        } else {
            self.punct_offset = None;
            let range = curr.text_range();
            (SynToken::Ordinary(curr), range)
        };

        Some(token)
    }

    fn peek(&self) -> Option<Self::Token> {
        if let Some((punct, mut offset)) = self.punct_offset.clone() {
            offset += TextSize::of('.');
            if usize::from(offset) < punct.text().len() {
                return Some(SynToken::Punch(punct, offset));
            }
        }

        if let Some(synth_token) = self.current_synthetic.last() {
            return Some(SynToken::Synthetic(synth_token.clone()));
        }

        let curr = self.current.clone()?;
        if !self.range.contains_range(curr.text_range()) {
            return None;
        }

        let token = if curr.kind().is_punct() {
            SynToken::Punch(curr, 0.into())
        } else {
            SynToken::Ordinary(curr)
        };
        Some(token)
    }

    fn id_alloc(&mut self) -> &mut TokenIdAlloc {
        &mut self.id_alloc
    }
}

struct TtTreeSink<'a> {
    buf: String,
    cursor: Cursor<'a>,
    open_delims: FxHashMap<tt::TokenId, TextSize>,
    text_pos: TextSize,
    inner: SyntaxTreeBuilder,
    token_map: TokenMap,
}

impl<'a> TtTreeSink<'a> {
    fn new(cursor: Cursor<'a>) -> Self {
        TtTreeSink {
            buf: String::new(),
            cursor,
            open_delims: FxHashMap::default(),
            text_pos: 0.into(),
            inner: SyntaxTreeBuilder::default(),
            token_map: TokenMap::default(),
        }
    }

    fn finish(mut self) -> (Parse<SyntaxNode>, TokenMap) {
        self.token_map.shrink_to_fit();
        (self.inner.finish(), self.token_map)
    }
}

fn delim_to_str(d: tt::DelimiterKind, closing: bool) -> &'static str {
    let texts = match d {
        tt::DelimiterKind::Parenthesis => "()",
        tt::DelimiterKind::Brace => "{}",
        tt::DelimiterKind::Bracket => "[]",
    };

    let idx = closing as usize;
    &texts[idx..texts.len() - (1 - idx)]
}

impl<'a> TtTreeSink<'a> {
    fn token(&mut self, kind: SyntaxKind, mut n_tokens: u8) {
        if kind == LIFETIME_IDENT {
            n_tokens = 2;
        }

        let mut last = self.cursor;
        for _ in 0..n_tokens {
            let tmp: u8;
            if self.cursor.eof() {
                break;
            }
            last = self.cursor;
            let text: &str = loop {
                break match self.cursor.token_tree() {
                    Some(tt::buffer::TokenTreeRef::Leaf(leaf, _)) => {
                        // Mark the range if needed
                        let (text, id) = match leaf {
                            tt::Leaf::Ident(ident) => (ident.text.as_str(), ident.id),
                            tt::Leaf::Punct(punct) => {
                                assert!(punct.char.is_ascii());
                                tmp = punct.char as u8;
                                (std::str::from_utf8(std::slice::from_ref(&tmp)).unwrap(), punct.id)
                            }
                            tt::Leaf::Literal(lit) => (lit.text.as_str(), lit.id),
                        };
                        let range = TextRange::at(self.text_pos, TextSize::of(text));
                        self.token_map.insert(id, range);
                        self.cursor = self.cursor.bump();
                        text
                    }
                    Some(tt::buffer::TokenTreeRef::Subtree(subtree, _)) => {
                        self.cursor = self.cursor.subtree().unwrap();
                        match subtree.delimiter {
                            Some(d) => {
                                self.open_delims.insert(d.id, self.text_pos);
                                delim_to_str(d.kind, false)
                            }
                            None => continue,
                        }
                    }
                    None => {
                        let parent = self.cursor.end().unwrap();
                        self.cursor = self.cursor.bump();
                        match parent.delimiter {
                            Some(d) => {
                                if let Some(open_delim) = self.open_delims.get(&d.id) {
                                    let open_range = TextRange::at(*open_delim, TextSize::of('('));
                                    let close_range =
                                        TextRange::at(self.text_pos, TextSize::of('('));
                                    self.token_map.insert_delim(d.id, open_range, close_range);
                                }
                                delim_to_str(d.kind, true)
                            }
                            None => continue,
                        }
                    }
                };
            };
            self.buf += text;
            self.text_pos += TextSize::of(text);
        }

        self.inner.token(kind, self.buf.as_str());
        self.buf.clear();
        // Add whitespace between adjoint puncts
        let next = last.bump();
        if let (
            Some(tt::buffer::TokenTreeRef::Leaf(tt::Leaf::Punct(curr), _)),
            Some(tt::buffer::TokenTreeRef::Leaf(tt::Leaf::Punct(_), _)),
        ) = (last.token_tree(), next.token_tree())
        {
            // Note: We always assume the semi-colon would be the last token in
            // other parts of RA such that we don't add whitespace here.
            if curr.spacing == tt::Spacing::Alone && curr.char != ';' {
                self.inner.token(WHITESPACE, " ");
                self.text_pos += TextSize::of(' ');
            }
        }
    }

    fn start_node(&mut self, kind: SyntaxKind) {
        self.inner.start_node(kind);
    }

    fn finish_node(&mut self) {
        self.inner.finish_node();
    }

    fn error(&mut self, error: String) {
        self.inner.error(error, self.text_pos)
    }
}
